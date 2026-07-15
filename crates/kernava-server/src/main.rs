// kernava-server: MCP server binary (HTTP + stdio transports, tool router, session manager)

use clap::{Parser, Subcommand};
use kernava_server as lib;

#[derive(Parser)]
#[command(
    name = "kernava",
    version,
    about = "World's fastest code intelligence MCP server"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start MCP server (streamable HTTP by default; --transport stdio for child-process mode)
    Serve {
        /// Transport: "http" (streamable HTTP listener) or "stdio" (stdin/stdout JSON-RPC,
        /// the mode MCP clients use when spawning this binary as a child process).
        #[arg(long, default_value = "http")]
        transport: String,
        #[arg(long, default_value = "8080")]
        port: u16,
        #[arg(long, default_value = "kernava.db")]
        db_path: String,
        #[arg(long, default_value = ".")]
        project_root: String,
    },
    /// Index a project from CLI (no server needed)
    Index {
        #[arg(long)]
        path: String,
        #[arg(long, default_value = "kernava.db")]
        db_path: String,
    },
    /// Print index statistics
    Stats {
        #[arg(long, default_value = "kernava.db")]
        db_path: String,
    },
    /// Run a single query tool (for debugging/scripting)
    Query {
        /// Tool name: search_symbols, get_symbol, get_callers, get_callees, etc.
        tool: String,
        /// JSON arguments for the tool (e.g. '{"query":"add"}')
        #[arg(long)]
        args: Option<String>,
        #[arg(long, default_value = "kernava.db")]
        db_path: String,
        #[arg(long, default_value = ".")]
        project_root: String,
    },
}

fn main() -> anyhow::Result<()> {
    // Direct tracing logs to STDERR. This is mandatory for the stdio transport:
    // MCP clients read JSON-RPC responses from our stdout, and any log line on
    // stdout would corrupt the protocol stream. For the HTTP transport stderr
    // is also the conventional place for server logs (kept out of the HTTP
    // response body).
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .try_init();
    let cli = Cli::parse();

    match cli.command {
        Commands::Serve {
            transport,
            port,
            db_path,
            project_root,
        } => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(async move {
                match transport.as_str() {
                    "stdio" => lib::serve_stdio(&db_path, &project_root).await,
                    "http" => lib::serve_async(port, &db_path, &project_root).await,
                    other => anyhow::bail!(
                        "unknown --transport {other:?}; expected \"http\" or \"stdio\""
                    ),
                }
            })
        }
        Commands::Index { path, db_path } => lib::index_cmd(&path, &db_path),
        Commands::Stats { db_path } => lib::stats_cmd(&db_path),
        Commands::Query {
            tool,
            args,
            db_path,
            project_root,
        } => lib::query_cmd(&tool, &db_path, &project_root, &args),
    }
}
