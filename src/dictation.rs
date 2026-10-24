//! Dictation: tap Right ⌘ and talk, tap it again and the words are pasted into the app in
//! front (or hold it and let go, push-to-talk). No screen is recorded and no bundle is written;
//! the microphone is recorded as for a capture (`AudioRecorder`, so OpenAI or Apple Speech
//! streams while you talk when configured) and the transcript goes straight to the clipboard
//! and a ⌘V. Esc cancels, as does any key pressed while Right ⌘ is still held from the start.
//! Off by default (`"dictation": true`; Settings › General).

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};

use crate::audio::AudioRecorder;

/// Released sooner than this, the press was a tap: the dictation stays on until the next tap.
/// Held longer, it's push-to-talk and the release finishes it.
pub const TAP_MS: u64 = 400;

pub struct Session {
    audio: AudioRecorder,
    started: Instant,
}

/// What a finished dictation produced.
#[derive(Debug)]
pub struct Finished {
    pub text: String,
    pub duration_ms: u64,
    pub source: String,
    pub transcribe_ms: u64,
}

impl Session {
    pub fn start() -> Result<Self> {
        Ok(Self {
            audio: AudioRecorder::start()?,
            started: Instant::now(),
        })
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// Stops the microphone and transcribes what it heard (blocking; call off the UI thread).
    pub fn finish(self) -> Result<Finished> {
        let audio = self.audio.stop().context("Microphone")?;
        let duration_ms = audio.duration_ms;
        // Apple's batch recognizer wants a file; it's removed once read.
        let dir = scratch_dir();
        let wav = dir.join("audio.wav");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(&wav, &audio.wav_bytes)?;
        let started = Instant::now();
        let transcript = crate::transcribe::transcribe(Some(&wav), Some(&audio));
        let _ = std::fs::remove_dir_all(&dir);
        Ok(Finished {
            text: transcript.text.split_whitespace().collect::<Vec<_>>().join(" "),
            duration_ms,
            source: transcript.source,
            transcribe_ms: started.elapsed().as_millis() as u64,
        })
    }

    pub fn cancel(self) {
        let _ = self.audio.stop();
    }
}

fn scratch_dir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "scrawl-dictation-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    ))
}

/// Puts `text` on the clipboard and pastes it into the app in front. Without Accessibility the
/// ⌘V can't be posted, so the text stays on the clipboard for a ⌘V by hand.
pub fn paste(text: &str) -> Result<()> {
    crate::handoff::copy_string_to_clipboard(text)?;
    if !crate::capture::accessibility_granted() {
        crate::capture::scrawl_log("Dictation: Accessibility is off; the words are on the clipboard");
        return Ok(());
    }
    crate::capture::wait_for_modifiers_released(350);
    std::thread::sleep(std::time::Duration::from_millis(20));
    crate::capture::post_synthetic_cmd_v();
    Ok(())
}
