//! Startup banner formatting helpers.
//!
//! All functions in this module are pure (no I/O). The actual banner
//! printing lives in `main.rs::print_banner`, which composes these helpers
//! and applies TTY / `NO_COLOR` policy.

use std::net::SocketAddr;

/// Width (in display columns) of `LOGO`. The art is fixed-width ASCII +
/// box-drawing characters, all single-column.
pub const LOGO_WIDTH: usize = 62;

/// Multi-line ASCII-art rustyice logo. Exactly `LOGO_WIDTH` columns wide.
pub const LOGO: &str = "██████╗ ██╗   ██╗███████╗████████╗██╗   ██╗██╗ ██████╗███████╗\n██╔══██╗██║   ██║██╔════╝╚══██╔══╝╚██╗ ██╔╝██║██╔════╝██╔════╝\n██████╔╝██║   ██║███████╗   ██║    ╚████╔╝ ██║██║     █████╗  \n██╔══██╗██║   ██║╚════██║   ██║     ╚██╔╝  ██║██║     ██╔══╝  \n██║  ██║╚██████╔╝███████║   ██║      ██║   ██║╚██████╗███████╗\n╚═╝  ╚═╝ ╚═════╝ ╚══════╝   ╚═╝      ╚═╝   ╚═╝ ╚═════╝╚══════╝";

/// Browser-clickable URL for the admin interface. Unspecified bind addresses
/// (`0.0.0.0`, `::`) render as `localhost` because browsers won't navigate to
/// the "any" address. IPv6 addresses keep their `[…]` bracketing.
#[must_use]
pub fn admin_url(admin_bind: SocketAddr) -> String {
    if admin_bind.ip().is_unspecified() {
        format!("http://localhost:{}/", admin_bind.port())
    } else {
        format!("http://{admin_bind}/")
    }
}

/// Wrap `text` in an OSC 8 terminal hyperlink escape pointing at `url`.
/// Terminals that don't support OSC 8 silently drop the escape and show
/// `text` verbatim.
#[must_use]
pub fn hyperlink(url: &str, text: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
}

/// Leading spaces needed to center `visible_width` columns in a field of
/// `total_width` columns. Saturates to 0 when text is wider than the field.
#[must_use]
pub fn center_pad(visible_width: usize, total_width: usize) -> usize {
    total_width.saturating_sub(visible_width) / 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logo_width_matches_each_line() {
        for line in LOGO.lines() {
            assert_eq!(
                line.chars().count(),
                LOGO_WIDTH,
                "line is not {LOGO_WIDTH} cols: {line:?}"
            );
        }
    }

    #[test]
    fn admin_url_unspecified_ipv4_becomes_localhost() {
        let addr: SocketAddr = "0.0.0.0:8080".parse().unwrap();
        assert_eq!(admin_url(addr), "http://localhost:8080/");
    }

    #[test]
    fn admin_url_unspecified_ipv6_becomes_localhost() {
        let addr: SocketAddr = "[::]:8080".parse().unwrap();
        assert_eq!(admin_url(addr), "http://localhost:8080/");
    }

    #[test]
    fn admin_url_loopback_ipv4_is_kept() {
        let addr: SocketAddr = "127.0.0.1:8000".parse().unwrap();
        assert_eq!(admin_url(addr), "http://127.0.0.1:8000/");
    }

    #[test]
    fn admin_url_loopback_ipv6_is_kept_with_brackets() {
        let addr: SocketAddr = "[::1]:8000".parse().unwrap();
        assert_eq!(admin_url(addr), "http://[::1]:8000/");
    }

    #[test]
    fn hyperlink_wraps_text_in_osc8() {
        assert_eq!(
            hyperlink("http://localhost:8080/", "admin: http://localhost:8080/"),
            "\x1b]8;;http://localhost:8080/\x1b\\admin: http://localhost:8080/\x1b]8;;\x1b\\"
        );
    }

    #[test]
    fn center_pad_centers_shorter_text() {
        assert_eq!(center_pad(10, 62), 26);
    }

    #[test]
    fn center_pad_saturates_when_text_is_wider() {
        assert_eq!(center_pad(100, 62), 0);
    }
}
