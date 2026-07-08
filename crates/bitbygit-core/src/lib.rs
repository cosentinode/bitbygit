pub const APP_NAME: &str = "bitbygit";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_app_identity() {
        assert_eq!(APP_NAME, "bitbygit");
        assert!(!VERSION.is_empty());
    }
}
