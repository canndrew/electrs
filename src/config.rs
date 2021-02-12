use bitcoin::Network;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

mod internal {
    #![allow(unused)]
    include!(concat!(env!("OUT_DIR"), "/configure_me_config.rs"));
}

/// This newtype implements `ParseArg` for `Network`.
#[derive(Deserialize)]
pub struct BitcoinNetwork(Network);

impl Default for BitcoinNetwork {
    fn default() -> Self {
        BitcoinNetwork(Network::Bitcoin)
    }
}

impl FromStr for BitcoinNetwork {
    type Err = <Network as FromStr>::Err;

    fn from_str(string: &str) -> std::result::Result<Self, Self::Err> {
        Network::from_str(string).map(BitcoinNetwork)
    }
}

impl ::configure_me::parse_arg::ParseArgFromStr for BitcoinNetwork {
    fn describe_type<W: std::fmt::Write>(mut writer: W) -> std::fmt::Result {
        write!(writer, "either 'bitcoin', 'testnet' or 'regtest'")
    }
}

impl Into<Network> for BitcoinNetwork {
    fn into(self) -> Network {
        self.0
    }
}

/// Configuration for electrs
#[derive(Debug)]
pub struct Config {
    pub network: Network,
    pub electrum_rpc_addr: SocketAddr,
    pub daemon_rpc_addr: SocketAddr,
    pub daemon_p2p_addr: SocketAddr,
    pub daemon_cookie_file: PathBuf,
    pub monitoring_addr: SocketAddr,
    pub db_path: PathBuf,
    pub daemon_dir: PathBuf,
    pub wait_duration: Duration,
    pub index_batch_size: usize,
    pub args: Vec<String>,
}

impl Config {
    pub fn from_args() -> Self {
        use internal::ResultExt;
        let (config, args) =
            internal::Config::including_optional_config_files(&["electrs.toml"]).unwrap_or_exit();
        let args = args.map(|a| a.into_string().unwrap()).collect();

        let electrum_port = match config.network {
            Network::Bitcoin => 50001,
            Network::Testnet => 60001,
            Network::Regtest => 60401,
            Network::Signet => 60601,
        };
        let electrum_rpc_addr: SocketAddr = ([127, 0, 0, 1], electrum_port).into();

        let daemon_rpc_port = match config.network {
            Network::Bitcoin => 8332,
            Network::Testnet => 18332,
            Network::Regtest => 18443,
            Network::Signet => 38332,
        };
        let daemon_rpc_addr: SocketAddr = ([127, 0, 0, 1], daemon_rpc_port).into();

        let daemon_p2p_port = match config.network {
            Network::Bitcoin => 8333,
            Network::Testnet => 18333,
            Network::Regtest => 18444,
            Network::Signet => 38333,
        };
        let daemon_p2p_addr: SocketAddr = ([127, 0, 0, 1], daemon_p2p_port).into();

        let monitoring_port = match config.network {
            Network::Bitcoin => 4224,
            Network::Testnet => 14224,
            Network::Regtest => 24224,
            Network::Signet => 34224,
        };
        let monitoring_addr: SocketAddr = ([127, 0, 0, 1], monitoring_port).into();

        let daemon_dir: PathBuf = config.daemon_dir;
        let daemon_dir = match config.network {
            Network::Bitcoin => daemon_dir,
            Network::Testnet => daemon_dir.join("testnet3"),
            Network::Regtest => daemon_dir.join("regtest"),
            Network::Signet => daemon_dir.join("signet"),
        };

        let config = Self {
            network: config.network,
            electrum_rpc_addr,
            daemon_rpc_addr,
            daemon_p2p_addr,
            daemon_cookie_file: daemon_dir.join(".cookie"),
            monitoring_addr,
            db_path: config.db_dir.join(config.network.to_string()),
            daemon_dir,
            wait_duration: Duration::from_secs(10),
            index_batch_size: config.index_batch_size,
            args,
        };
        eprintln!("{:?}", config);

        env_logger::Builder::from_default_env()
            .default_format()
            .format_timestamp_millis()
            .init();

        config
    }
}
