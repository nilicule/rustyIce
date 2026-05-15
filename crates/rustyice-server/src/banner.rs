//! Startup banner formatting helpers.
//!
//! All functions in this module are pure (no I/O). The actual banner
//! printing lives in `main.rs::print_banner`, which composes these helpers
//! and applies TTY / `NO_COLOR` policy.

/// Width (in display columns) of `LOGO`. The art is fixed-width ASCII +
/// box-drawing characters, all single-column.
pub const LOGO_WIDTH: usize = 62;

/// Multi-line ASCII-art rustyice logo. Exactly `LOGO_WIDTH` columns wide.
pub const LOGO: &str = "██████╗ ██╗   ██╗███████╗████████╗██╗   ██╗██╗ ██████╗███████╗\n██╔══██╗██║   ██║██╔════╝╚══██╔══╝╚██╗ ██╔╝██║██╔════╝██╔════╝\n██████╔╝██║   ██║███████╗   ██║    ╚████╔╝ ██║██║     █████╗  \n██╔══██╗██║   ██║╚════██║   ██║     ╚██╔╝  ██║██║     ██╔══╝  \n██║  ██║╚██████╔╝███████║   ██║      ██║   ██║╚██████╗███████╗\n╚═╝  ╚═╝ ╚═════╝ ╚══════╝   ╚═╝      ╚═╝   ╚═╝ ╚═════╝╚══════╝";

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
}
