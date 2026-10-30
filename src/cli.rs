use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "chat-gateway-rs",
    about = "Chat Gateway v2 - Obscura native, unified memory over 8 providers"
)]
pub struct Cli {
    #[arg(long, default_value = "config.yaml")]
    pub config: std::path::PathBuf,

    #[arg(
        long,
        conflicts_with_all = ["all", "test_adapter", "create_api_key", "vault_list", "vault_delete_provider", "vault_rotate_key", "vault_restore_backup"],
        help = "Print effective personal-memory egress policy for every provider and exit"
    )]
    pub show_memory_egress_policy: bool,

    #[arg(
        long,
        conflicts_with = "all",
        help = "Test one adapter live: discover models + show selectors, then exit"
    )]
    pub test_adapter: Option<String>,

    #[arg(
        long,
        conflicts_with = "test_adapter",
        help = "Run live diagnostics for every enabled provider"
    )]
    pub all: bool,

    #[arg(long, help = "Emit machine-readable JSON for adapter diagnostics")]
    pub json: bool,

    #[arg(
        long,
        value_name = "NAME",
        help = "Create a random chat API key, print it once and exit"
    )]
    pub create_api_key: Option<String>,

    #[arg(
        long,
        conflicts_with_all = ["all", "test_adapter", "create_api_key", "vault_delete_provider", "vault_rotate_key"],
        help = "List provider vault records and whether each is valid with CHAT_GATEWAY_VAULT_KEY"
    )]
    pub vault_list: bool,

    #[arg(
        long,
        value_name = "PROVIDER",
        conflicts_with_all = ["all", "test_adapter", "create_api_key", "vault_list", "vault_rotate_key"],
        help = "Delete the named provider's persisted session snapshot (vault and legacy file)"
    )]
    pub vault_delete_provider: Option<String>,

    #[arg(
        long,
        conflicts_with_all = ["all", "test_adapter", "create_api_key", "vault_list", "vault_delete_provider"],
        help = "Rotate encrypted vault records using CHAT_GATEWAY_VAULT_KEY and CHAT_GATEWAY_VAULT_NEW_KEY"
    )]
    pub vault_rotate_key: bool,

    #[arg(
        long,
        value_name = "BACKUP_DIR",
        conflicts_with_all = ["all", "test_adapter", "create_api_key", "vault_list", "vault_delete_provider", "vault_rotate_key"],
        help = "Restore encrypted vault records from a sessions/.vault-rotation-* backup using the old key"
    )]
    pub vault_restore_backup: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::Parser;

    #[test]
    fn defaults_preserve_the_config_file_contract() {
        let cli = Cli::try_parse_from(["chat-gateway-rs"]).expect("default CLI parses");
        assert_eq!(cli.config, std::path::PathBuf::from("config.yaml"));
        assert!(!cli.all);
        assert!(!cli.show_memory_egress_policy);
        assert!(cli.test_adapter.is_none());
    }

    #[test]
    fn diagnostics_modes_remain_mutually_exclusive() {
        assert!(
            Cli::try_parse_from(["chat-gateway-rs", "--test-adapter", "chatgpt", "--all"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from(["chat-gateway-rs", "--vault-list", "--vault-rotate-key"])
                .is_err()
        );
    }

    #[test]
    fn vault_restore_accepts_a_backup_directory() {
        let cli = Cli::try_parse_from([
            "chat-gateway-rs",
            "--vault-restore-backup",
            "sessions/.vault-rotation-test",
        ])
        .expect("vault restore arguments parse");
        assert_eq!(
            cli.vault_restore_backup.as_deref(),
            Some("sessions/.vault-rotation-test")
        );
    }
}
