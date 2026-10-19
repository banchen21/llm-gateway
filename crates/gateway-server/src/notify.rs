//! Surfaces a network problem where the operator will notice it.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// One toast per ten minutes is enough to notice without becoming noise.
const REPEAT_AFTER: Duration = Duration::from_secs(600);

static LAST: Mutex<Option<Instant>> = Mutex::new(None);

/// Tells the operator the network — not their request — is the problem.
///
/// Throttled: a gateway that retries every few seconds must not fill the notification
/// centre with the same sentence.
pub fn network_problem(message: &str) {
    if !claim_slot() {
        return;
    }
    #[cfg(windows)]
    show(message);
    #[cfg(not(windows))]
    let _ = message;
}

fn claim_slot() -> bool {
    let mut last = LAST.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let now = Instant::now();
    if let Some(previous) = *last {
        if now.duration_since(previous) < REPEAT_AFTER {
            return false;
        }
    }
    *last = Some(now);
    true
}

/// Shows a Windows toast through PowerShell, which owns a registered app identity — an
/// unregistered process cannot post one by itself.
#[cfg(windows)]
fn show(message: &str) {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let text = escape(message);
    let script = format!(
        "$ErrorActionPreference='Stop'; \
         [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null; \
         [Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime] | Out-Null; \
         $xml = New-Object Windows.Data.Xml.Dom.XmlDocument; \
         $xml.LoadXml('<toast><visual><binding template=\"ToastGeneric\"><text>LLM Gateway</text><text>{text}</text></binding></visual></toast>'); \
         $toast = [Windows.UI.Notifications.ToastNotification]::new($xml); \
         [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('{{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}}\\WindowsPowerShell\\v1.0\\powershell.exe').Show($toast)"
    );
    // `-EncodedCommand` is UTF-16LE base64, so the message survives whatever code page
    // the process was started under; `-Command` would mangle non-ASCII text.
    let encoded = encode_utf16_base64(&script);
    let _ = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-EncodedCommand",
            &encoded,
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

/// PowerShell's `-EncodedCommand` format: UTF-16LE bytes, base64 encoded.
#[cfg(windows)]
fn encode_utf16_base64(script: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(windows)]
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::claim_slot;

    #[test]
    fn the_toast_is_throttled() {
        assert!(claim_slot(), "the first call should be allowed");
        assert!(!claim_slot(), "an immediate repeat should be suppressed");
    }

    /// PowerShell decodes `-EncodedCommand` as UTF-16LE base64; `aABpAA==` is "hi".
    #[cfg(windows)]
    #[test]
    fn the_command_is_utf16_base64() {
        assert_eq!(super::encode_utf16_base64("hi"), "aABpAA==");
    }
}
