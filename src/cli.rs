use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "RTSP-TUI",
    version,
    about = "High-performance terminal RTSP camera viewer with grid mode and network discovery"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Open interactive TUI control center.
    Tui,
    /// View one or more RTSP streams in a terminal grid.
    View(ViewArgs),
    /// Scan a local network and discover likely RTSP stream URLs.
    Discover(DiscoverArgs),
    /// Work with previously discovered streams.
    Streams(StreamsArgs),
}

#[derive(Debug, Args)]
pub struct ViewArgs {
    /// RTSP stream URL(s). Repeat the flag or pass a comma-separated list.
    #[arg(short, long = "stream", required = true, num_args = 1.., value_delimiter = ',')]
    pub streams: Vec<String>,

    /// Fixed number of grid rows (0 = auto).
    #[arg(long, default_value_t = 0)]
    pub rows: u16,

    /// Fixed number of grid columns (0 = auto).
    #[arg(long, default_value_t = 0)]
    pub cols: u16,

    /// Terminal refresh rate.
    #[arg(long, default_value_t = 24)]
    pub refresh_fps: u16,

    /// Maximum stream-to-terminal conversion rate per stream.
    #[arg(long, default_value_t = 15)]
    pub target_fps: u16,

    /// RTSP transport protocol to request.
    #[arg(long, value_enum, default_value_t = TransportMode::Tcp)]
    pub transport: TransportMode,

    /// Scale factor for vertical sampling (helps character aspect ratio).
    #[arg(long, default_value_t = 2)]
    pub vertical_scale: u16,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, ValueEnum)]
pub enum TransportMode {
    Tcp,
    Udp,
}

#[derive(Debug, Args)]
pub struct DiscoverArgs {
    /// CIDR block(s) to scan. If omitted, auto-detects local IPv4 subnets.
    #[arg(long = "cidr", value_delimiter = ',')]
    pub cidrs: Vec<String>,

    /// RTSP TCP ports to probe.
    #[arg(long = "port", value_delimiter = ',', default_values_t = default_ports())]
    pub ports: Vec<u16>,

    /// Candidate RTSP paths to probe.
    #[arg(long = "path", value_delimiter = ',', default_values_t = default_paths())]
    pub paths: Vec<String>,

    /// Optional username to include in probe URLs.
    #[arg(long)]
    pub username: Option<String>,

    /// Optional password to include in probe URLs.
    #[arg(long)]
    pub password: Option<String>,

    /// Per-probe timeout in milliseconds.
    #[arg(long, default_value_t = 700)]
    pub timeout_ms: u64,

    /// Number of concurrent probe tasks.
    #[arg(long, default_value_t = 256)]
    pub concurrency: usize,

    /// Maximum number of hosts to scan per subnet.
    #[arg(long, default_value_t = 2048)]
    pub max_hosts_per_subnet: usize,

    /// Do not save discovery results to local cache.
    #[arg(long, default_value_t = false)]
    pub no_save: bool,

    /// Also discover camera IPs via ONVIF WS-Discovery multicast.
    #[arg(long, default_value_t = false)]
    pub onvif: bool,
}

#[derive(Debug, Args)]
pub struct StreamsArgs {
    #[command(subcommand)]
    pub command: StreamsCommand,
}

#[derive(Debug, Subcommand)]
pub enum StreamsCommand {
    /// List discovered streams stored in the local cache.
    List(ListArgs),
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Print machine-readable JSON.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[must_use]
pub fn default_ports() -> Vec<u16> {
    vec![554, 10554, 8554]
}

#[must_use]
pub fn default_paths() -> Vec<String> {
    vec![
        "/".to_owned(),
        "/live".to_owned(),
        "/stream1".to_owned(),
        "/stream2".to_owned(),
        "/h264".to_owned(),
        "/cam/realmonitor?channel=1&subtype=0".to_owned(),
        "/Streaming/Channels/101".to_owned(),
        "/h264Preview_01_main".to_owned(),
    ]
}
