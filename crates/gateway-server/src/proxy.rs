//! Reads the proxy the operating system is configured to use.
//!
//! The gateway connects directly by default and only asks for this after a connection
//! fails, so a working proxy does not quietly change how domestic upstreams are reached.

/// The proxy Windows is set to use, formatted for `reqwest`.
///
/// Windows stores one of `host:port`, `http=host:port;https=host:port`, or nothing at all.
/// Every upstream the gateway talks to is HTTPS, so the `https` entry wins; a bare value
/// applies to both.
#[cfg(windows)]
pub fn system_proxy() -> Option<String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let settings = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings")
        .ok()?;
    let enabled: u32 = settings.get_value("ProxyEnable").unwrap_or(0);
    if enabled == 0 {
        return None;
    }
    let server: String = settings.get_value("ProxyServer").ok()?;
    parse(&server)
}

#[cfg(not(windows))]
pub fn system_proxy() -> Option<String> {
    None
}

/// Whether the process itself was given a proxy through its environment.
///
/// That proxy is already baked into the shared client, so the system proxy is only worth
/// trying when this is false.
pub fn from_env() -> bool {
    [
        "ALL_PROXY",
        "all_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ]
    .iter()
    .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
}

/// Turns the registry's proxy string into a URL, or `None` when it names no HTTP proxy.
///
/// Only the Windows build calls this; the tests below exercise it everywhere, so the
/// parsing stays covered on CI's Linux runner too.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse(server: &str) -> Option<String> {
    let server = server.trim();
    if server.is_empty() {
        return None;
    }
    let mut http = None;
    let mut https = None;
    for part in server.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix("https=") {
            https = Some(value.trim());
        } else if let Some(value) = part.strip_prefix("http=") {
            http = Some(value.trim());
        } else if !part.contains('=') {
            // A bare host:port applies to every protocol.
            return Some(format!("http://{part}"));
        }
    }
    https.or(http).map(|value| format!("http://{value}"))
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn a_bare_address_applies_to_everything() {
        assert_eq!(
            parse("127.0.0.1:7890").as_deref(),
            Some("http://127.0.0.1:7890")
        );
    }

    #[test]
    fn the_https_entry_wins_when_both_are_present() {
        assert_eq!(
            parse("http=10.0.0.2:8080;https=10.0.0.3:8443").as_deref(),
            Some("http://10.0.0.3:8443")
        );
    }

    #[test]
    fn an_empty_setting_names_no_proxy() {
        assert_eq!(parse("  "), None);
        assert_eq!(parse("socks=127.0.0.1:1080"), None);
    }
}
