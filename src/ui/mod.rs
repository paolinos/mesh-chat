pub mod cli;

#[cfg(test)]
mod tests {
    #[test]
    fn cli_module_is_wired_in() {
        assert_eq!(super::cli::parse_input("/quit"), super::cli::InputAction::Quit);
    }
}
