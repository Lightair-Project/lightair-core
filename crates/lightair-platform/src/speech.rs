//! The system's own text-to-speech voice: free, offline, no account. macOS: the `say` tool;
//! Windows: the built-in speech synthesizer (System.Speech, through PowerShell). Each call makes
//! the whole text as a 16-bit mono WAV file at 24 kHz.

use std::sync::atomic::{AtomicU64, Ordering};

/// A voice the system has.
#[derive(Clone, Debug, PartialEq)]
pub struct SystemVoice {
    /// What to pass to [`synthesize`].
    pub id: String,
    pub name: String,
    /// Its language, as the system says it ("en_US", "en-US").
    pub language: String,
}

/// The sample rate [`synthesize`] makes.
pub const RATE: u32 = 24_000;

static NEXT: AtomicU64 = AtomicU64::new(1);

#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
fn temp_wav() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "lightair-say-{}-{}.wav",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// The voices installed.
pub fn voices() -> Vec<SystemVoice> {
    imp::voices()
}

/// `text` spoken by `voice` (empty: the system's default) at `speed` (1.0 normal), as WAV bytes.
pub fn synthesize(text: &str, voice: &str, speed: f32) -> Result<Vec<u8>, String> {
    if text.trim().is_empty() {
        return Err("nothing to say".into());
    }
    imp::synthesize(text, voice, speed)
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    pub fn voices() -> Vec<SystemVoice> {
        let Ok(out) = Command::new("/usr/bin/say").arg("-v").arg("?").output() else { return Vec::new() };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| {
                let (head, _) = line.split_once('#')?;
                let head = head.trim_end();
                // "Name (Variant)   en_US": the language is the last word
                let (name, lang) = head.rsplit_once(char::is_whitespace)?;
                let name = name.trim();
                (!name.is_empty()).then(|| SystemVoice {
                    id: name.to_string(),
                    name: name.to_string(),
                    language: lang.trim().to_string(),
                })
            })
            .collect()
    }

    pub fn synthesize(text: &str, voice: &str, speed: f32) -> Result<Vec<u8>, String> {
        let out = temp_wav();
        let mut cmd = Command::new("/usr/bin/say");
        if !voice.is_empty() {
            cmd.arg("-v").arg(voice);
        }
        let rate = (180.0 * speed.clamp(0.5, 2.0)).round() as u32;
        cmd.arg("-r")
            .arg(rate.to_string())
            .arg("--file-format=WAVE")
            .arg(format!("--data-format=LEI16@{RATE}"))
            .arg("-o")
            .arg(&out)
            .arg("-f")
            .arg("-");
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("the system voice: {e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let done = child.wait_with_output().map_err(|e| format!("the system voice: {e}"))?;
        let bytes = std::fs::read(&out);
        let _ = std::fs::remove_file(&out);
        if !done.status.success() {
            let why = String::from_utf8_lossy(&done.stderr).trim().to_string();
            return Err(format!(
                "the system voice didn't speak{}",
                if why.is_empty() { String::new() } else { format!(": {why}") }
            ));
        }
        bytes.map_err(|e| format!("the system voice: {e}"))
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    fn powershell(script: &str, env: &[(&str, String)]) -> Result<std::process::Output, String> {
        use std::os::windows::process::CommandExt;
        let mut cmd = Command::new("powershell");
        cmd.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", "-"]);
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("the system voice: {e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(script.as_bytes());
        }
        child.wait_with_output().map_err(|e| format!("the system voice: {e}"))
    }

    pub fn voices() -> Vec<SystemVoice> {
        let script = "Add-Type -AssemblyName System.Speech\n$s = New-Object System.Speech.Synthesis.SpeechSynthesizer\n$s.GetInstalledVoices() | ForEach-Object { $_.VoiceInfo.Name + '|' + $_.VoiceInfo.Culture.Name }\n";
        let Ok(out) = powershell(script, &[]) else { return Vec::new() };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                let (name, lang) = l.trim().split_once('|')?;
                Some(SystemVoice { id: name.to_string(), name: name.to_string(), language: lang.to_string() })
            })
            .collect()
    }

    pub fn synthesize(text: &str, voice: &str, speed: f32) -> Result<Vec<u8>, String> {
        let out = temp_wav();
        let rate = (((speed.clamp(0.5, 2.0) - 1.0) * 10.0).round() as i32).clamp(-10, 10);
        let script = "Add-Type -AssemblyName System.Speech\n\
            $s = New-Object System.Speech.Synthesis.SpeechSynthesizer\n\
            if ($env:LIGHTAIR_SAY_VOICE) { try { $s.SelectVoice($env:LIGHTAIR_SAY_VOICE) } catch {} }\n\
            $s.Rate = [int]$env:LIGHTAIR_SAY_RATE\n\
            $f = New-Object System.Speech.AudioFormat.SpeechAudioFormatInfo(24000, [System.Speech.AudioFormat.AudioBitsPerSample]::Sixteen, [System.Speech.AudioFormat.AudioChannel]::Mono)\n\
            $s.SetOutputToWaveFile($env:LIGHTAIR_SAY_OUT, $f)\n\
            $s.Speak($env:LIGHTAIR_SAY_TEXT)\n\
            $s.Dispose()\n";
        let done = powershell(
            script,
            &[
                ("LIGHTAIR_SAY_TEXT", text.to_string()),
                ("LIGHTAIR_SAY_VOICE", voice.to_string()),
                ("LIGHTAIR_SAY_RATE", rate.to_string()),
                ("LIGHTAIR_SAY_OUT", out.to_string_lossy().into_owned()),
            ],
        )?;
        let bytes = std::fs::read(&out);
        let _ = std::fs::remove_file(&out);
        if !done.status.success() {
            return Err(format!("the system voice didn't speak: {}", String::from_utf8_lossy(&done.stderr).trim()));
        }
        bytes.map_err(|e| format!("the system voice: {e}"))
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod imp {
    use super::*;

    pub fn voices() -> Vec<SystemVoice> {
        Vec::new()
    }

    pub fn synthesize(_text: &str, _voice: &str, _speed: f32) -> Result<Vec<u8>, String> {
        Err("there's no system voice on this platform yet".into())
    }
}
